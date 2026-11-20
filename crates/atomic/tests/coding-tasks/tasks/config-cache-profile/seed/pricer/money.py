"""Amounts as Decimal, rounded half up, never as float."""

from decimal import ROUND_HALF_UP, Decimal, InvalidOperation


def parse_price(text):
    """A catalog price such as "9.55", as a Decimal with two places."""
    text = text.strip()
    try:
        value = Decimal(text)
    except InvalidOperation:
        raise ValueError(f"not a price: {text!r}") from None
    if not value.is_finite() or value < 0:
        raise ValueError(f"not a price: {text!r}")
    if value.as_tuple().exponent < -2:
        raise ValueError(f"more than two decimal places: {text!r}")
    return value.quantize(Decimal("0.01"))


def round_to(amount, decimals):
    """Round half up to a number of decimal places."""
    return amount.quantize(Decimal(1).scaleb(-decimals), rounding=ROUND_HALF_UP)


def percent_of(amount, pct):
    """pct percent of amount, not rounded."""
    return amount * pct / Decimal(100)


def format_amount(amount, decimals, currency):
    """1234.5 with 2 places in EUR is '1,234.50 EUR'."""
    return f"{round_to(amount, decimals):,.{decimals}f} {currency}"
