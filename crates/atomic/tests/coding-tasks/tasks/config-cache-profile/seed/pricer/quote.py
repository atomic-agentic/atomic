"""Pricing one job: a profile, a product and a quantity."""

from dataclasses import dataclass

from . import discounts, settings
from .money import format_amount, percent_of, round_to


class JobError(Exception):
    """A job line that cannot be read."""


@dataclass(frozen=True)
class Job:
    profile: str
    sku: str
    quantity: int


@dataclass(frozen=True)
class Quote:
    job: Job
    currency: str
    decimals: int
    net: object
    tax: object
    total: object
    discount_note: str


def parse_job(line):
    """'eu WIDGET-S 3' as a Job."""
    parts = line.split()
    if len(parts) != 3:
        raise JobError(f"expected PROFILE SKU QUANTITY, got {line.strip()!r}")
    profile, sku, quantity = parts
    if not quantity.isdigit() or int(quantity) == 0:
        raise JobError(f"not a quantity: {quantity!r}")
    return Job(profile=profile.lower(), sku=sku.upper(), quantity=int(quantity))


def price_job(job, catalog, config_dir):
    item = catalog.get(job.sku)
    rules = settings.load_settings(config_dir, profile=job.profile)
    base = item.price * job.quantity
    discount = discounts.bulk_discount(base, job.quantity, rules)
    net = round_to((base - discount) * rules.fx_rate, rules.decimals)
    tax_amount = round_to(percent_of(net, rules.tax_rate), rules.decimals)
    return Quote(
        job=job,
        currency=rules.currency,
        decimals=rules.decimals,
        net=net,
        tax=tax_amount,
        total=net + tax_amount,
        discount_note=discounts.describe(rules) if discount else "",
    )


def format_quote(quote):
    def money(amount):
        return format_amount(amount, quote.decimals, quote.currency)

    job = quote.job
    return (
        f"{job.profile} {job.sku} x{job.quantity}: net {money(quote.net)}, tax {money(quote.tax)}, "
        f"total {money(quote.total)}{quote.discount_note}"
    )
