"""Subtotal, taxes and total of an invoice."""

from . import money, pricing, taxes
from .model import Totals


def compute_totals(invoice):
    """The amounts printed on an invoice: each line's, the subtotal, each tax and the total."""
    amounts = []
    raw_subtotal = money.zero()
    raw_by_code = {}
    for line in invoice.lines:
        amounts.append(pricing.line_amount(line))
        # The unrounded amount, so that the totals lose no precision.
        raw = pricing.raw_amount(line)
        raw_subtotal += raw
        raw_by_code[line.tax] = raw_by_code.get(line.tax, money.zero()) + raw
    tax_amounts = []
    for code in taxes.in_order(raw_by_code):
        tax = money.round_cents(money.percent_of(raw_by_code[code], taxes.rate_for(code)))
        tax_amounts.append((code, tax))
    subtotal = money.round_cents(raw_subtotal)
    total = money.round_cents(raw_subtotal + sum(amount for _, amount in tax_amounts))
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
