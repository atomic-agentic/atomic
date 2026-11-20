"""Money helpers: parsing, rounding and formatting amounts.

Amounts are held in the invoice currency's major unit (euros, pounds, ...). Everything that touches an
amount's representation lives here, so the rest of the package never formats or rounds by itself.
"""

import re

_NUMBER = re.compile(r"^-?[0-9]+(\.[0-9]+)?$")


def parse_amount(text):
    """An amount from a string such as "12.50", "3" or "0.0125"."""
    if not isinstance(text, str):
        raise ValueError(f"expected a string, got {type(text).__name__}")
    cleaned = text.strip()
    if not _NUMBER.match(cleaned):
        raise ValueError(f"not a plain decimal number: {text!r}")
    return float(cleaned)


def parse_percent(text):
    """A percentage from 0 to 100, from a string."""
    value = parse_amount(text)
    if value < 0 or value > 100:
        raise ValueError(f"a percentage must be from 0 to 100: {text!r}")
    return value


def zero():
    return 0.0


def percent_of(value, percent):
    """percent% of value, unrounded."""
    return value * percent / 100


def round_cents(value):
    """value rounded to the cent."""
    return round(value, 2)


def format_amount(value):
    """Two decimals, with a comma between thousands: 1,234.50."""
    return f"{value:,.2f}"


def plain(value):
    """Two decimals, without separators: 1234.50. For machine-readable output."""
    return f"{value:.2f}"


def format_unit(value):
    """A unit price, with two to four decimals: 4.10, 4.115, 0.0125."""
    text = f"{value:.4f}".rstrip("0")
    whole, _, decimals = text.partition(".")
    return f"{whole}.{decimals.ljust(2, '0')}"


def format_number(value):
    """A quantity or a percentage, without trailing zeros: 3, 1.5, 12.5."""
    return f"{value:g}"
