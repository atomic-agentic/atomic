"""The invoice as plain data. No arithmetic here: see pricing and totals."""

from dataclasses import dataclass


@dataclass(frozen=True)
class Customer:
    id: str
    name: str


@dataclass(frozen=True)
class Line:
    sku: str
    description: str
    quantity: object  # an amount, as money.parse_amount returns it
    unit_price: object  # an amount
    discount: object  # a percentage, as money.parse_percent returns it
    tax: str  # a code in taxes.RATES


@dataclass(frozen=True)
class Invoice:
    number: str
    date: str
    currency: str
    customer: Customer
    lines: tuple
    notes: str = ""


@dataclass(frozen=True)
class Totals:
    lines: tuple  # each line's amount, in the invoice's order
    subtotal: object
    taxes: tuple  # (code, amount) pairs, in taxes.RATES order, for the codes in use
    total: object
