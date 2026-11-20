"""The printed invoice."""

from . import money, pricing, taxes

WIDTH = 72
FOOTER_LABEL = 44


def render(invoice, totals):
    """The invoice as text, with its amounts from totals.compute_totals."""
    out = [
        f"INVOICE {invoice.number}".ljust(WIDTH - len(invoice.date)) + invoice.date,
        f"Customer: {invoice.customer.name} ({invoice.customer.id})",
        "",
        _header(),
        "-" * WIDTH,
    ]
    for line, amount in zip(invoice.lines, totals.lines):
        out.append(_row(line, amount))
    out.append("-" * WIDTH)
    out.append(_footer("Subtotal", totals.subtotal))
    for code, amount in totals.taxes:
        out.append(_footer(f"Tax {taxes.label(code)}", amount))
    out.append(_footer(f"Total {invoice.currency}", totals.total))
    if invoice.notes:
        out += ["", invoice.notes]
    return "\n".join(out) + "\n"


def _header():
    return f"{'SKU':<10} {'Description':<24} {'Qty':>7} {'Unit':>10} {'Disc':>6} {'Amount':>10}"


def _row(line, amount):
    description = line.description
    if len(description) > 24:
        description = description[:23] + "~"
    return (
        f"{line.sku[:10]:<10} {description:<24} {money.format_number(line.quantity):>7} "
        f"{money.format_unit(line.unit_price):>10} {pricing.discount_label(line):>6} "
        f"{money.format_amount(amount):>10}"
    )


def _footer(label, amount):
    return " " * FOOTER_LABEL + f"{label:<16}" + f"{money.format_amount(amount):>12}"
