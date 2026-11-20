"""Reading and checking invoice files. Every problem is a ValueError that says where it is."""

import json
import re

from . import money, taxes
from .model import Customer, Invoice, Line

REQUIRED = ("number", "date", "currency", "customer", "lines")
_DATE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
_CURRENCY = re.compile(r"^[A-Z]{3}$")


def load(path):
    """The invoice in a JSON file."""
    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except OSError as e:
        raise ValueError(f"cannot read {path}: {e.strerror}") from None
    except json.JSONDecodeError as e:
        raise ValueError(f"{path} is not JSON: {e.msg} at line {e.lineno}") from None
    return from_dict(data)


def from_dict(data):
    """An invoice from the decoded JSON."""
    if not isinstance(data, dict):
        raise ValueError("an invoice must be a JSON object")
    missing = [key for key in REQUIRED if key not in data]
    if missing:
        raise ValueError(f"missing {', '.join(missing)}")
    number = _text(data, "number", "invoice")
    date = _text(data, "date", "invoice")
    if not _DATE.match(date):
        raise ValueError(f"invoice: date must be YYYY-MM-DD: {date!r}")
    currency = _text(data, "currency", "invoice")
    if not _CURRENCY.match(currency):
        raise ValueError(f"invoice: currency must be three capital letters: {currency!r}")
    customer = _customer(data["customer"])
    lines = data["lines"]
    if not isinstance(lines, list) or not lines:
        raise ValueError("an invoice needs at least one line")
    parsed = tuple(_line(index, raw) for index, raw in enumerate(lines, 1))
    notes = data.get("notes", "")
    if not isinstance(notes, str):
        raise ValueError("invoice: notes must be a string")
    return Invoice(number=number, date=date, currency=currency, customer=customer, lines=parsed, notes=notes)


def _text(raw, key, where):
    value = raw.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{where}: {key} must be a non-empty string")
    return value.strip()


def _customer(raw):
    if not isinstance(raw, dict):
        raise ValueError("customer must be an object")
    return Customer(id=_text(raw, "id", "customer"), name=_text(raw, "name", "customer"))


def _line(index, raw):
    where = f"line {index}"
    if not isinstance(raw, dict):
        raise ValueError(f"{where}: must be an object")
    sku = _text(raw, "sku", where)
    description = _text(raw, "description", where)
    quantity = _amount(raw, "quantity", where)
    unit_price = _amount(raw, "unit_price", where)
    try:
        discount = money.parse_percent(raw.get("discount", "0"))
    except ValueError as e:
        raise ValueError(f"{where}: discount: {e}") from None
    tax = raw.get("tax", "std")
    if not taxes.is_code(tax):
        raise ValueError(f"{where}: unknown tax code {tax!r}")
    return Line(sku=sku, description=description, quantity=quantity, unit_price=unit_price,
                discount=discount, tax=tax)


def _amount(raw, key, where):
    if key not in raw:
        raise ValueError(f"{where}: missing {key}")
    try:
        value = money.parse_amount(raw[key])
    except ValueError as e:
        raise ValueError(f"{where}: {key}: {e}") from None
    if value < 0:
        raise ValueError(f"{where}: {key} must not be negative")
    return value
