"""Tax codes and their rates, as percentages. The order here is the order taxes are printed in."""

from . import money

RATES = {
    "std": "20",
    "low": "12.5",
    "reduced": "5.5",
    "zero": "0",
}


def is_code(code):
    return code in RATES


def rate_for(code):
    """The rate of a code, as a percentage."""
    try:
        return money.parse_percent(RATES[code])
    except KeyError:
        raise ValueError(f"unknown tax code {code!r}") from None


def in_order(codes):
    """The given codes, in the order of RATES."""
    wanted = set(codes)
    return [code for code in RATES if code in wanted]


def label(code):
    return f"{code} {RATES[code]}%"
