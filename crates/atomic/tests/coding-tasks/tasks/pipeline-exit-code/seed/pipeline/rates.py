"""Fixed exchange rates to euros, as finance sets them for the month."""

from decimal import ROUND_HALF_UP, Decimal

RATES_TO_EUR = {
    "EUR": Decimal("1"),
    "USD": Decimal("0.92"),
    "GBP": Decimal("1.17"),
    "SEK": Decimal("0.087"),
    "CHF": Decimal("1.04"),
}

CENT = Decimal("0.01")


def to_eur(amount, currency):
    """amount (a decimal string or Decimal) in currency, in euros, to the cent. KeyError for an unknown currency."""
    rate = RATES_TO_EUR[currency]
    return (Decimal(amount) * rate).quantize(CENT, rounding=ROUND_HALF_UP)
