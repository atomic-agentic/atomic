"""Grouping events into the site's local days."""

from dataclasses import dataclass, field
from decimal import Decimal

from .clock import local_day


@dataclass
class DayTotal:
    events: int = 0
    sales: Decimal = field(default_factory=lambda: Decimal("0.00"))
    refunds: Decimal = field(default_factory=lambda: Decimal("0.00"))

    @property
    def net(self):
        return self.sales - self.refunds

    def add(self, event):
        self.events += 1
        if event.kind == "sale":
            self.sales += event.amount
        else:
            self.refunds += event.amount


def daily_totals(events, offset):
    """{day: DayTotal} for every local day with events, oldest day first."""
    days = {}
    for event in events:
        days.setdefault(local_day(event.at, offset), DayTotal()).add(event)
    return dict(sorted(days.items()))


def grand_total(totals):
    out = DayTotal()
    for day in totals.values():
        out.events += day.events
        out.sales += day.sales
        out.refunds += day.refunds
    return out
