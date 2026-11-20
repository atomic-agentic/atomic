"""The amount of one invoice line."""

from . import money


def gross(line):
    """Quantity times unit price, before any discount."""
    return line.quantity * line.unit_price


def raw_amount(line):
    """The line's amount after its discount, before rounding."""
    value = gross(line)
    return value - money.percent_of(value, line.discount)


def line_amount(line):
    """The line's amount after its discount, rounded to the cent, as printed."""
    return money.round_cents(raw_amount(line))


def discount_label(line):
    """How the discount column shows the line's discount: "" for none, else "15%"."""
    if not line.discount:
        return ""
    return f"{money.format_number(line.discount)}%"
