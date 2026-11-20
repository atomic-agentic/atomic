"""Per-customer totals in euros."""

import logging
from decimal import Decimal

from pipeline.rates import to_eur

log = logging.getLogger("pipeline.totals")

TOTAL_COLUMNS = ("customer_id", "name", "orders", "total_eur")


def compute_totals(orders, customers):
    """Total each customer's orders in euros.

    orders and customers are lists of row dicts. Returns the rows for totals.csv, sorted by customer id.
    An order in a currency with no rate is logged as an error and left out of its total.
    """
    names = {row["customer_id"]: row["name"] for row in customers}
    sums = {}
    counts = {}
    for order in orders:
        try:
            eur = to_eur(order["amount"], order["currency"])
        except KeyError:
            log.error("order %s: no rate for %s", order["order_id"], order["currency"])
            continue
        customer = order["customer_id"]
        sums[customer] = sums.get(customer, Decimal("0")) + eur
        counts[customer] = counts.get(customer, 0) + 1
    rows = []
    for customer in sorted(sums):
        rows.append({
            "customer_id": customer,
            "name": names.get(customer, "?"),
            "orders": str(counts[customer]),
            "total_eur": f"{sums[customer]:.2f}",
        })
    return rows
