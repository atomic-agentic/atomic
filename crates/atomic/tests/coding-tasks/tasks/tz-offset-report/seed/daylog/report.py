"""The text report: a heading and one row a local day."""

from .buckets import grand_total

HEADER = f"{'day':<10} {'events':>7} {'sales':>10} {'refunds':>10} {'net':>10}"


def render(site, totals):
    total = grand_total(totals)
    noun = "event" if total.events == 1 else "events"
    lines = [f"{site.name} (UTC{site.offset_text}): {total.events} {noun}", HEADER]
    for day, row in totals.items():
        lines.append(f"{day.isoformat():<10} {row.events:>7} {row.sales:>10.2f} {row.refunds:>10.2f} {row.net:>10.2f}")
    return "\n".join(lines) + "\n"
