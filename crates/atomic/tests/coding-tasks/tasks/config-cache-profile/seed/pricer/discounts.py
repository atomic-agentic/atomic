"""Discount rules. Only the bulk discount exists for now."""

from decimal import Decimal

from .money import percent_of


def bulk_discount(base, quantity, settings):
    """The amount the bulk rule takes off base, in USD, not rounded: zero below the threshold."""
    if settings.bulk_threshold <= 0 or quantity < settings.bulk_threshold:
        return Decimal(0)
    return percent_of(base, settings.bulk_discount_pct)


def describe(settings):
    """The suffix a discounted quote line ends with."""
    pct = settings.bulk_discount_pct.normalize()
    return f" (bulk -{pct:f}%)"
